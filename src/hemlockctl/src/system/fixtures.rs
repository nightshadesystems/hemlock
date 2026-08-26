//! Deterministic system-suite state for the golden tests, built from
//! the spec's Part 1.1 seed.

use super::model::{ActiveSession, ConfiguredUser, LogEntry, LoggingState, UsersState};

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

/// 2026-08-25 10:41:12 UTC — the seed log tail.
const LOG_NEWEST: i64 = 1_787_654_472;

pub fn logging_state() -> LoggingState {
    LoggingState {
        level: "informational".into(),
        hosts: vec![
            "10.42.0.30:514 (udp)".into(),
            "10.42.0.31:6514 (tcp)".into(),
        ],
        entries: vec![
            LogEntry {
                time: LOG_NEWEST - 137,
                host: "hemlock-a1".into(),
                tag: "webd".into(),
                pid: 977,
                message: "session opened for cody from 10.42.0.100".into(),
                severity: 6,
            },
            LogEntry {
                time: LOG_NEWEST - 70,
                host: "hemlock-a1".into(),
                tag: "orch".into(),
                pid: 901,
                message: "lldp: neighbor core-sw-01 on Et49 refreshed".into(),
                severity: 6,
            },
            LogEntry {
                time: LOG_NEWEST,
                host: "hemlock-a1".into(),
                tag: "mgmtd".into(),
                pid: 812,
                message: "commit 0 applied by cody (cli)".into(),
                severity: 6,
            },
        ],
        requested: 50,
        journal_available: true,
    }
}
