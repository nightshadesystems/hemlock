//! EOS-style text renderers for the system-suite show family.

use crate::interfaces::table::{Col, Text};

use super::model::{CableDiagState, CommitsState, ImageState, LoggingState, UsersState};

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
    out.row(&SESSIONS, &["User", "From", "Client", "Idle", "Login Time"]);
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

/// `show logging` — where the box forwards, then the tail of its own
/// journal. Newest last, so the screen reads the way a log does.
pub fn logging(state: &LoggingState) -> String {
    let mut out = Text::new();
    out.line(format!("Logging level: {}", state.level));
    out.line(format!(
        "Remote hosts : {}",
        if state.hosts.is_empty() {
            "none (local journal only)".to_string()
        } else {
            state.hosts.join(", ")
        }
    ));
    out.blank();

    if !state.journal_available {
        out.line("% the system journal is not readable here");
        return out.finish();
    }
    for entry in &state.entries {
        let tag = match (entry.tag.as_str(), entry.pid) {
            ("", _) => String::new(),
            (tag, 0) => format!(" {tag}:"),
            (tag, pid) => format!(" {tag}[{pid}]:"),
        };
        let host = if entry.host.is_empty() {
            String::new()
        } else {
            format!(" {}", entry.host)
        };
        out.line(format!(
            "{}{host}{tag} {}",
            stamp_signed(entry.time),
            entry.message
        ));
    }
    if state.entries.is_empty() {
        out.line("(the journal is empty)");
    }
    out.line(format!(
        "(last {} lines; `show logging {}` for more)",
        state.entries.len(),
        state.requested.saturating_mul(4).max(200)
    ));
    out.finish()
}

/// [`stamp`] for a journal timestamp, which the IPC carries signed.
fn stamp_signed(unix: i64) -> String {
    match u64::try_from(unix) {
        Ok(unix) => stamp(unix),
        Err(_) => "-".into(),
    }
}

/// A metadata field the ring may not carry: an entry written before the
/// metadata existed renders `-` rather than an empty column.
fn or_dash(text: &str) -> &str {
    if text.is_empty() {
        "-"
    } else {
        text
    }
}

/// `show system commits` — the commit history the rollback ring keeps.
pub fn commits(state: &CommitsState) -> String {
    const COLS: [Col; 5] = [
        Col::left(5),
        Col::left(21),
        Col::left(8),
        Col::left(8),
        Col::left(26),
    ];
    let mut out = Text::new();
    out.row(&COLS, &["Idx", "Time", "User", "Client", "Comment"]);
    out.row(
        &COLS,
        &[
            "---",
            "-------------------",
            "------",
            "------",
            "--------------------------",
        ],
    );
    for commit in &state.commits {
        let comment = if commit.index == 0 {
            // The running config is not a rollback target; saying so is
            // more useful than an empty comment column.
            "(current)".to_string()
        } else if commit.comment.is_empty() {
            "-".to_string()
        } else {
            commit.comment.clone()
        };
        out.row(
            &COLS,
            &[
                &commit.index.to_string(),
                &stamp_or_dash(commit.time),
                or_dash(&commit.user),
                or_dash(&commit.client),
                &comment,
            ],
        );
    }
    if state.commits.is_empty() {
        out.line("(no commits recorded)");
    }
    out.finish()
}

/// A stamp for an entry that may carry no recorded time.
pub fn stamp_or_dash(unix: i64) -> String {
    if unix <= 0 {
        return "-".into();
    }
    stamp_signed(unix)
}

/// `show system image` — what runs now and what boots next.
pub fn image(state: &ImageState) -> String {
    let mut out = Text::new();
    let field = |name: &str| format!("{:<15}", name);
    let installed = if state.installed_at > 0 {
        format!(" (installed {})", stamp_or_dash(state.installed_at))
    } else {
        String::new()
    };
    out.line(format!(
        "{}: {}{installed}",
        field("Current image"),
        or_dash(&state.version)
    ));
    out.line(format!(
        "{}: {}",
        field("Image file"),
        or_dash(&state.image_file)
    ));
    out.line(format!("{}: {}", field("Kernel"), or_dash(&state.kernel)));
    if !state.platform.is_empty() {
        out.line(format!("{}: {}", field("Platform"), state.platform));
    }
    out.line(format!(
        "{}: {}",
        field("Next boot"),
        or_dash(&state.next_boot)
    ));
    out.line(format!(
        "{}: {}",
        field("ONIE rescue"),
        if state.onie_rescue_armed {
            "armed for the next boot"
        } else {
            "not armed"
        }
    ));
    out.finish()
}

/// `show interfaces <port> cable-diagnostics` — the last TDR sweep.
pub fn cable_diagnostics(state: &CableDiagState) -> String {
    const COLS: [Col; 3] = [Col::left(6), Col::left(11), Col::left(7)];
    let mut out = Text::new();
    out.line(format!(
        "{} cable diagnostics (run {}):",
        state.port,
        stamp_or_dash(state.run_at)
    ));
    out.row(&COLS, &["Pair", "Status", "Length"]);
    out.row(&COLS, &["----", "---------", "-------"]);
    for pair in &state.pairs {
        out.row(
            &COLS,
            &[
                &pair.pair,
                &pair.state,
                // A pair the PHY would not measure has no length to
                // print, which is not the same as a zero-metre run.
                &if pair.length_m == 0 {
                    "-".to_string()
                } else {
                    format!("{} m", pair.length_m)
                },
            ],
        );
    }
    out.finish()
}
