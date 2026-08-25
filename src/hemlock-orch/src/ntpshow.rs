//! Live NTP client state via `timedatectl`.
//!
//! mgmtd renders the timesyncd drop-in; orch owns the query surface —
//! the same split the FRR families use (mgmtd renders `frr.conf`,
//! `frrshow` asks vtysh). hemlockctl and webd ask orch, so the
//! `timedatectl` parsing lives in exactly one place.
//!
//! Parsing is defensive, like `frrshow`'s: three commands, each
//! optional. `timedatectl show` carries the enabled/synchronized
//! flags, `show-timesync --all` the machine-readable server, poll
//! interval and last-reply timestamp, and `timesync-status` the one
//! field the machine-readable form omits — the clock offset. A missing
//! command or field leaves its value at zero rather than failing the
//! whole query, so a switch with timesyncd stopped still renders.

use hemlock_common::proto::v1 as pb;

/// Run one `timedatectl` subcommand; None when it is absent or fails
/// (a dev host, or timesyncd not installed).
async fn timedatectl(args: &[&str]) -> Option<String> {
    // TZ=UTC pins the timestamp zone `secs_since` parses; without it
    // timesyncd's tooling prints the local zone abbreviation, which
    // needs a tz database to read back.
    let output = tokio::process::Command::new("timedatectl")
        .args(args)
        .env("TZ", "UTC")
        .output()
        .await
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `Key=value` lines into a lookup (the `show`/`show-timesync` form).
fn properties(text: &str) -> std::collections::HashMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect()
}

/// `Label: value` lines into a lookup (the `timesync-status` form).
fn labels(text: &str) -> std::collections::HashMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect()
}

/// One `NTPMessage={ Leap=0, ..., Stratum=3, ... }` field.
fn ntp_message_field(message: &str, field: &str) -> Option<String> {
    message
        .trim_start_matches('{')
        .trim_end_matches('}')
        .split(',')
        .filter_map(|entry| entry.split_once('='))
        .find(|(key, _)| key.trim() == field)
        .map(|(_, value)| value.trim().to_string())
}

/// A systemd duration ("8min 32s", "1.204ms", "1us") in microseconds.
/// Terms with an unknown unit contribute nothing rather than poisoning
/// the total, so an unparsable field reads as zero, never as garbage.
pub fn parse_usecs(text: &str) -> u64 {
    fn scale(unit: &str) -> Option<f64> {
        Some(match unit {
            "us" | "usec" | "\u{b5}s" => 1.0,
            "ms" | "msec" => 1_000.0,
            "s" | "sec" | "" => 1_000_000.0,
            "min" | "m" => 60.0 * 1_000_000.0,
            "h" | "hr" => 3_600.0 * 1_000_000.0,
            "d" | "day" | "days" => 86_400.0 * 1_000_000.0,
            _ => return None,
        })
    }
    // Scan into (number, unit) terms: a digit after a unit starts the
    // next one ("8min 32s", with or without the space).
    let mut terms: Vec<(String, String)> = Vec::new();
    let mut number = String::new();
    let mut unit = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() || (ch == '.' && !number.is_empty() && unit.is_empty()) {
            if !unit.is_empty() {
                terms.push((std::mem::take(&mut number), std::mem::take(&mut unit)));
            }
            number.push(ch);
        } else if ch.is_whitespace() {
            if !number.is_empty() {
                terms.push((std::mem::take(&mut number), std::mem::take(&mut unit)));
            }
        } else {
            unit.push(ch);
        }
    }
    if !number.is_empty() {
        terms.push((number, unit));
    }
    let mut total = 0f64;
    for (number, unit) in terms {
        if let (Ok(value), Some(scale)) = (number.parse::<f64>(), scale(&unit)) {
            total += value * scale;
        }
    }
    if total.is_finite() && total >= 0.0 {
        total.round() as u64
    } else {
        0
    }
}

/// A signed systemd duration, for the clock offset ("-412us").
pub fn parse_offset_usecs(text: &str) -> i64 {
    let (negative, rest) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.trim_start_matches('+')),
    };
    let magnitude = i64::try_from(parse_usecs(rest)).unwrap_or(i64::MAX);
    if negative {
        -magnitude
    } else {
        magnitude
    }
}

/// Seconds between an RFC-ish systemd timestamp ("Sat 2026-08-24
/// 21:14:07 UTC") and now. None when it cannot be read.
fn secs_since(stamp: &str, now: chrono::DateTime<chrono::Utc>) -> Option<u64> {
    // Drop the weekday, keep "<date> <time> <zone>".
    let rest = stamp.split_once(' ').map(|(_, rest)| rest).unwrap_or(stamp);
    let (datetime, zone) = rest.rsplit_once(' ')?;
    // Only UTC timestamps are unambiguous without a tz database,
    // which is why the commands run with TZ=UTC.
    if zone != "UTC" {
        return None;
    }
    let parsed =
        chrono::NaiveDateTime::parse_from_str(datetime.trim(), "%Y-%m-%d %H:%M:%S").ok()?;
    let then = parsed.and_utc();
    u64::try_from((now - then).num_seconds()).ok()
}

/// The NTP client's state for `show ntp`.
pub async fn ntp_state() -> pb::GetNtpStateResponse {
    let mut state = pb::GetNtpStateResponse::default();

    if let Some(text) = timedatectl(&["show", "--all"]).await {
        let props = properties(&text);
        state.enabled = props.get("NTP").map(String::as_str) == Some("yes");
        state.synchronized = props.get("NTPSynchronized").map(String::as_str) == Some("yes");
    }

    if let Some(text) = timedatectl(&["show-timesync", "--all"]).await {
        let props = properties(&text);
        state.servers = props
            .get("SystemNTPServers")
            .map(|list| {
                list.split_whitespace()
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        // The name is what was configured; the address is what it
        // resolved to. Show the configured spelling when there is one.
        state.server = props
            .get("ServerName")
            .filter(|name| !name.is_empty())
            .or_else(|| props.get("ServerAddress"))
            .cloned()
            .unwrap_or_default();
        if let Some(interval) = props.get("PollIntervalUSec") {
            state.poll_interval_secs =
                u32::try_from(parse_usecs(interval) / 1_000_000).unwrap_or(0);
        }
        if let Some(message) = props.get("NTPMessage") {
            state.stratum = ntp_message_field(message, "Stratum")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            state.delay_usecs = ntp_message_field(message, "RootDelay")
                .map(|v| parse_usecs(&v))
                .unwrap_or(0);
            state.jitter_usecs = ntp_message_field(message, "Jitter")
                .map(|v| parse_usecs(&v))
                .unwrap_or(0);
            state.last_sync_secs_ago = ntp_message_field(message, "DestinationTimestamp")
                .and_then(|stamp| secs_since(&stamp, chrono::Utc::now()));
        }
    }

    // The offset is the one field only the human-readable form carries.
    if let Some(text) = timedatectl(&["timesync-status"]).await {
        let fields = labels(&text);
        if let Some(offset) = fields.get("Offset") {
            state.offset_usecs = parse_offset_usecs(offset);
        }
        // Delay and jitter come from here too when the NTPMessage
        // parse found nothing (older systemd spells them differently).
        if state.delay_usecs == 0 {
            state.delay_usecs = fields.get("Delay").map(|v| parse_usecs(v)).unwrap_or(0);
        }
        if state.jitter_usecs == 0 {
            state.jitter_usecs = fields.get("Jitter").map(|v| parse_usecs(v)).unwrap_or(0);
        }
        if state.stratum == 0 {
            state.stratum = fields
                .get("Stratum")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        }
    }
    state
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn systemd_durations_parse() {
        assert_eq!(parse_usecs("1us"), 1);
        assert_eq!(parse_usecs("88us"), 88);
        assert_eq!(parse_usecs("1.204ms"), 1204);
        assert_eq!(parse_usecs("500ms"), 500_000);
        assert_eq!(parse_usecs("32s"), 32_000_000);
        assert_eq!(parse_usecs("8min 32s"), 512_000_000);
        assert_eq!(parse_usecs("34min 8s"), 2_048_000_000);
        assert_eq!(parse_usecs("1h 2min 3s"), 3_723_000_000);
        // Unknown units contribute nothing; junk is zero, not a panic.
        assert_eq!(parse_usecs(""), 0);
        assert_eq!(parse_usecs("banana"), 0);
        assert_eq!(parse_usecs("12furlongs"), 0);
    }

    /// The offset is the only signed field: the local clock can lead.
    #[test]
    fn offsets_keep_their_sign() {
        assert_eq!(parse_offset_usecs("-412us"), -412);
        assert_eq!(parse_offset_usecs("412us"), 412);
        assert_eq!(parse_offset_usecs("+1.5ms"), 1500);
        assert_eq!(parse_offset_usecs("-2s"), -2_000_000);
        assert_eq!(parse_offset_usecs(""), 0);
    }

    #[test]
    fn ntp_message_fields_split_out() {
        let message = "{ Leap=0, Version=4, Mode=4, Stratum=3, Precision=-24, \
                       RootDelay=1.204ms, RootDispersion=1.129ms, Reference=C0A80001, \
                       DestinationTimestamp=Sat 2026-08-24 21:14:07 UTC, Ignored=no, \
                       PacketCount=5, Jitter=88us }";
        assert_eq!(ntp_message_field(message, "Stratum").as_deref(), Some("3"));
        assert_eq!(
            ntp_message_field(message, "RootDelay").as_deref(),
            Some("1.204ms")
        );
        assert_eq!(
            ntp_message_field(message, "Jitter").as_deref(),
            Some("88us")
        );
        assert_eq!(ntp_message_field(message, "Nonesuch"), None);
    }

    #[test]
    fn timestamps_age_against_now() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-24T21:18:19Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            secs_since("Sat 2026-08-24 21:14:07 UTC", now),
            Some(4 * 60 + 12)
        );
        // A future timestamp (clock stepped backwards) is not an age.
        assert_eq!(secs_since("Sat 2026-08-24 21:20:00 UTC", now), None);
        // Non-UTC zones and junk degrade to "unknown".
        assert_eq!(secs_since("Sat 2026-08-24 21:14:07 CEST", now), None);
        assert_eq!(secs_since("never", now), None);
    }

    #[test]
    fn property_and_label_forms_both_parse() {
        let props = properties("NTP=yes\nNTPSynchronized=no\nTimezone=UTC\n");
        assert_eq!(props.get("NTP").map(String::as_str), Some("yes"));
        assert_eq!(props.get("NTPSynchronized").map(String::as_str), Some("no"));
        let fields = labels("       Server: 10.42.0.5 (10.42.0.5)\n       Offset: -412us\n");
        assert_eq!(fields.get("Offset").map(String::as_str), Some("-412us"));
        // The label form keeps only what precedes the first colon, so
        // an IPv6 value survives intact.
        let fields = labels("       Server: 2001:db8::1 (2001:db8::1)\n");
        assert_eq!(
            fields.get("Server").map(String::as_str),
            Some("2001:db8::1 (2001:db8::1)")
        );
    }
}
