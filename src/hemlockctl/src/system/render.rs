//! EOS-style text renderers for the system-suite show family.

use crate::interfaces::table::{Col, Text};

use super::model::UsersState;

/// A duration as `HH:MM:SS`, the shape an idle timer reads best in.
/// Days roll into the hours field rather than adding a unit.
pub fn clock(secs: u64) -> String {
    let (hours, rest) = (secs / 3600, secs % 3600);
    format!("{hours:02}:{:02}:{:02}", rest / 60, rest % 60)
}

/// A unix timestamp as `YYYY-MM-DD HH:MM:SS` (UTC, like every other
/// stamp the show family prints).
pub fn stamp(unix: u64) -> String {
    match chrono::DateTime::from_timestamp(i64::try_from(unix).unwrap_or(i64::MAX), 0) {
        Some(time) => time.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => "-".into(),
    }
}

/// `show system users` — the configured accounts, then who is on the
/// box right now.
pub fn users(state: &UsersState) -> String {
    const CONFIGURED: [Col; 4] = [Col::left(8), Col::left(10), Col::left(18), Col::left(8)];
    const SESSIONS: [Col; 5] = [
        Col::left(8),
        Col::left(17),
        Col::left(8),
        Col::left(10),
        Col::left(19),
    ];
    let mut out = Text::new();

    out.line("Configured users:");
    out.row(&CONFIGURED, &["Name", "Role", "Auth", "SSH Keys"]);
    out.row(
        &CONFIGURED,
        &["------", "--------", "----------------", "--------"],
    );
    if state.configured.is_empty() {
        out.line("(none — login accounts are not managed by the configuration)");
    }
    for user in &state.configured {
        out.row(
            &CONFIGURED,
            &[
                &user.name,
                &user.role,
                &user.auth,
                &user.ssh_keys.to_string(),
            ],
        );
    }

    out.blank();
    out.line("Active sessions:");
    out.row(
        &SESSIONS,
        &["User", "From", "Client", "Idle", "Login Time"],
    );
    out.row(
        &SESSIONS,
        &[
            "------",
            "---------------",
            "------",
            "--------",
            "-------------------",
        ],
    );
    if state.sessions.is_empty() {
        out.line("(none)");
    }
    for session in &state.sessions {
        out.row(
            &SESSIONS,
            &[
                &session.user,
                &session.from,
                &session.client,
                &clock(session.idle_secs),
                &stamp(session.login_time),
            ],
        );
    }

    if !state.unmanaged.is_empty() {
        out.blank();
        out.line(format!(
            "Unmanaged accounts (owned by the OS, not the configuration): {}",
            state.unmanaged.join(", ")
        ));
    }
    out.finish()
}
