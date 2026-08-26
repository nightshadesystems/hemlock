//! Reading the system journal.
//!
//! `show logging` and the web console's tail both want the last N lines
//! of the box's own log. The journal is systemd's, readable only by
//! root or the `systemd-journal` group, and an operator account is
//! neither — so mgmtd (which is root) reads it and serves it over IPC
//! rather than each front-end shelling out and failing differently.
//!
//! `journalctl -o json` is the parse target: the field names are
//! stable, the timestamps are unambiguous microseconds, and a message
//! containing whitespace or a newline cannot be mistaken for structure.

use serde::Deserialize;

/// Lines returned when the caller does not say.
pub const DEFAULT_LINES: u32 = 50;
/// The most lines one request may ask for. Past this the answer is a
/// log collector, not a bigger `show`.
pub const MAX_LINES: u32 = 5000;

/// Severity value meaning "the journal did not record one".
pub const NO_SEVERITY: u32 = 8;

/// One journal record, in the shape the IPC carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub time_unix: i64,
    pub host: String,
    pub tag: String,
    pub pid: u32,
    pub message: String,
    pub severity: u32,
}

/// The subset of `journalctl -o json` fields worth carrying. Every one
/// is optional: the journal omits what it does not know, and a record
/// from a unit that logged through the kernel has almost none of them.
///
/// `MESSAGE` is a string for ordinary text and an array of byte values
/// for a record with non-UTF-8 content, so it is taken untyped and
/// converted.
#[derive(Debug, Deserialize)]
struct Record {
    #[serde(rename = "__REALTIME_TIMESTAMP")]
    realtime: Option<String>,
    #[serde(rename = "_HOSTNAME")]
    hostname: Option<String>,
    #[serde(rename = "SYSLOG_IDENTIFIER")]
    syslog_identifier: Option<String>,
    #[serde(rename = "_COMM")]
    comm: Option<String>,
    #[serde(rename = "SYSLOG_PID")]
    syslog_pid: Option<String>,
    #[serde(rename = "_PID")]
    pid: Option<String>,
    #[serde(rename = "PRIORITY")]
    priority: Option<String>,
    #[serde(rename = "MESSAGE")]
    message: Option<serde_json::Value>,
}

/// Parse `journalctl -o json` output (one JSON object per line, oldest
/// first). Unparsable lines are skipped rather than failing the whole
/// read: a single malformed record must not blank the log view.
pub fn parse_journal_json(text: &str) -> Vec<Entry> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Record>(line).ok())
        .map(|record| Entry {
            time_unix: record
                .realtime
                .as_deref()
                .and_then(|micros| micros.parse::<i64>().ok())
                .map(|micros| micros / 1_000_000)
                .unwrap_or(0),
            host: record.hostname.unwrap_or_default(),
            tag: record.syslog_identifier.or(record.comm).unwrap_or_default(),
            pid: record
                .syslog_pid
                .or(record.pid)
                .and_then(|pid| pid.parse().ok())
                .unwrap_or(0),
            severity: record
                .priority
                .and_then(|priority| priority.parse().ok())
                .filter(|priority| *priority <= 7)
                .unwrap_or(NO_SEVERITY),
            message: message_text(record.message),
        })
        .collect()
}

/// A `MESSAGE` field as display text: a string as-is, an array of byte
/// values decoded, anything else empty.
fn message_text(message: Option<serde_json::Value>) -> String {
    match message {
        Some(serde_json::Value::String(text)) => text,
        Some(serde_json::Value::Array(bytes)) => {
            let bytes: Vec<u8> = bytes
                .iter()
                .filter_map(|value| value.as_u64())
                .map(|value| value as u8)
                .collect();
            String::from_utf8_lossy(&bytes).into_owned()
        }
        _ => String::new(),
    }
}

/// The last `count` journal lines, oldest first. `None` = the journal
/// could not be read (no journalctl, or it refused), which the caller
/// reports rather than showing an empty log as if the box were silent.
pub fn tail(count: u32) -> Option<Vec<Entry>> {
    let count = count.clamp(1, MAX_LINES);
    let output = std::process::Command::new("journalctl")
        .args(["--no-pager", "-o", "json", "-n", &count.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(parse_journal_json(&String::from_utf8_lossy(&output.stdout)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const JOURNAL: &str = concat!(
        r#"{"__REALTIME_TIMESTAMP":"1787654472000000","_HOSTNAME":"hemlock-a1","#,
        r#""SYSLOG_IDENTIFIER":"mgmtd","_PID":"812","PRIORITY":"6","#,
        r#""MESSAGE":"commit 0 applied by cody (cli)"}"#,
        "\n",
        r#"{"__REALTIME_TIMESTAMP":"1787654402000000","_HOSTNAME":"hemlock-a1","#,
        r#""_COMM":"orch","PRIORITY":"4","MESSAGE":"lldp: neighbor core-sw-01"}"#,
        "\n",
    );

    #[test]
    fn parses_journalctl_json() {
        let entries = parse_journal_json(JOURNAL);
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0],
            Entry {
                time_unix: 1_787_654_472,
                host: "hemlock-a1".into(),
                tag: "mgmtd".into(),
                pid: 812,
                message: "commit 0 applied by cody (cli)".into(),
                severity: 6,
            }
        );
        // `_COMM` stands in for a missing syslog identifier, and a
        // record with no pid reads as 0 rather than failing.
        assert_eq!(entries[1].tag, "orch");
        assert_eq!(entries[1].pid, 0);
        assert_eq!(entries[1].severity, 4);
    }

    /// A malformed line is skipped, not fatal: one bad record must not
    /// blank the whole log view.
    #[test]
    fn skips_unparsable_records() {
        let text = format!("not json\n{JOURNAL}\n\n");
        assert_eq!(parse_journal_json(&text).len(), 2);
        assert!(parse_journal_json("").is_empty());
    }

    /// A record the journal only has bytes for still renders.
    #[test]
    fn decodes_byte_array_messages() {
        let line = r#"{"MESSAGE":[104,105],"PRIORITY":"9"}"#;
        let entries = parse_journal_json(line);
        assert_eq!(entries[0].message, "hi");
        // An out-of-range priority is "the journal did not say".
        assert_eq!(entries[0].severity, NO_SEVERITY);
        assert_eq!(entries[0].time_unix, 0);
    }
}
