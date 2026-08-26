//! The system-suite data model: text renderers and the `| json`
//! serializer both consume these, so the two outputs can never drift.

use serde::Serialize;

/// One config-managed login user.
#[derive(Debug, Clone, Serialize)]
pub struct ConfiguredUser {
    pub name: String,
    /// "admin" | "operator".
    pub role: String,
    /// "password" | "ssh-key" | "none" — how the account authenticates.
    pub auth: String,
    pub ssh_keys: usize,
}

/// One live login, from either front-end.
#[derive(Debug, Clone, Serialize)]
pub struct ActiveSession {
    pub user: String,
    /// Peer address, or "console".
    pub from: String,
    /// "cli" | "web".
    pub client: String,
    pub role: String,
    pub idle_secs: u64,
    /// Unix seconds; rendered as a wall-clock stamp.
    pub login_time: u64,
}

/// `show system users`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct UsersState {
    pub configured: Vec<ConfiguredUser>,
    pub sessions: Vec<ActiveSession>,
    /// Login accounts the box carries that no config names — the OS's
    /// own, which Hemlock never creates, changes or removes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unmanaged: Vec<String>,
}
