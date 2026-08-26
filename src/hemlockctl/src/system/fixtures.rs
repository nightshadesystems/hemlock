//! Deterministic system-suite state for the golden tests, built from
//! the spec's Part 1.1 seed.

use super::model::{ActiveSession, ConfiguredUser, UsersState};

/// 2026-08-25 09:12:44 UTC — the seed's first login.
const LOGIN_CLI: u64 = 1_787_649_164;
/// 2026-08-25 08:55:02 UTC — the seed's web login.
const LOGIN_WEB: u64 = 1_787_648_102;

pub fn users_state() -> UsersState {
    UsersState {
        configured: vec![
            ConfiguredUser {
                name: "cody".into(),
                role: "admin".into(),
                auth: "password".into(),
                ssh_keys: 1,
            },
            ConfiguredUser {
                name: "noc".into(),
                role: "operator".into(),
                auth: "password".into(),
                ssh_keys: 0,
            },
        ],
        sessions: vec![
            ActiveSession {
                user: "cody".into(),
                from: "10.42.0.100".into(),
                client: "cli".into(),
                role: "admin".into(),
                idle_secs: 0,
                login_time: LOGIN_CLI,
            },
            ActiveSession {
                user: "cody".into(),
                from: "10.42.0.100".into(),
                client: "web".into(),
                role: "admin".into(),
                idle_secs: 252,
                login_time: LOGIN_WEB,
            },
        ],
        unmanaged: vec![],
    }
}
