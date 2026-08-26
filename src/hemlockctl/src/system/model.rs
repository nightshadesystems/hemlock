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

// ------------------------------------------------- logging

/// One journal line.
#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    /// Unix seconds; rendered as a wall-clock stamp.
    pub time: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub host: String,
    /// The syslog identifier, e.g. "mgmtd".
    #[serde(skip_serializing_if = "String::is_empty")]
    pub tag: String,
    /// 0 = the journal recorded none.
    pub pid: u32,
    pub message: String,
    /// Syslog severity 0..7; 8 = the journal did not say.
    pub severity: u32,
}

/// `show logging`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct LoggingState {
    /// The configured forwarding level.
    pub level: String,
    /// Pre-rendered `10.42.0.30:514 (udp)` collectors, in config order.
    pub hosts: Vec<String>,
    /// The journal tail, oldest first.
    pub entries: Vec<LogEntry>,
    /// How many lines were asked for, so the footer can say.
    pub requested: u32,
    /// False when the journal could not be read at all.
    pub journal_available: bool,
}

// ------------------------------------------------- commits

/// One entry of the commit history: index 0 is the running config,
/// 1..N the rollback ring, newest first.
#[derive(Debug, Clone, Serialize)]
pub struct Commit {
    pub index: u32,
    /// Unix seconds; 0 = the entry predates recorded metadata.
    pub time: i64,
    /// Empty = not recorded (an entry written before the system suite).
    pub user: String,
    /// "cli" | "web" | "system"; empty as above.
    pub client: String,
    pub comment: String,
}

/// `show system commits`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct CommitsState {
    pub commits: Vec<Commit>,
}

impl CommitsState {
    /// The entry `rollback <n>` would load, for the confirmation line.
    pub fn find(&self, index: u32) -> Option<&Commit> {
        self.commits.iter().find(|commit| commit.index == index)
    }
}

// ------------------------------------------------- image

/// `show system image`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct ImageState {
    pub version: String,
    /// Unix seconds; 0 = the install was not recorded.
    pub installed_at: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub image_file: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub kernel: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub platform: String,
    pub next_boot: String,
    pub onie_rescue_armed: bool,
}

// ------------------------------------------------- cable diagnostics

/// One pair of a TDR sweep.
#[derive(Debug, Clone, Serialize)]
pub struct CablePair {
    /// "A".."D", in wire order.
    pub pair: String,
    /// "ok" | "open" | "short" | "crosstalk" | "unknown".
    pub state: String,
    /// Metres: the run for a terminated pair, the distance to the fault
    /// otherwise. 0 = the PHY did not measure one.
    pub length_m: u32,
}

/// `show interfaces <port> cable-diagnostics`, and what `request
/// cable-diagnostics <port>` prints when it finishes.
#[derive(Debug, Clone, Serialize, Default)]
pub struct CableDiagState {
    pub port: String,
    /// False = no sweep has been run on this port since boot.
    pub has_result: bool,
    /// Unix seconds the sweep ran.
    pub run_at: i64,
    pub pairs: Vec<CablePair>,
}
