//! Value formatting shared by every `show interfaces` renderer.
//!
//! All EOS-parity formatting rules (durations, rates, percentages, VLAN
//! list compression, truncation) live here and are unit-tested; renderers
//! only compose them.

/// `1Gb/s`, `100Mb/s`, or `Unconfigured` — the detail-block speed.
pub fn speed_detail(mbps: Option<u64>) -> String {
    match mbps {
        None | Some(0) => "Unconfigured".into(),
        Some(m) if m >= 1000 && m % 1000 == 0 => format!("{}Gb/s", m / 1000),
        Some(m) => format!("{m}Mb/s"),
    }
}

/// `1G`, `100M`, or `unconf` — the tabular speed cell.
pub fn speed_tabular(mbps: Option<u64>) -> String {
    match mbps {
        None | Some(0) => "unconf".into(),
        Some(m) if m >= 1000 && m % 1000 == 0 => format!("{}G", m / 1000),
        Some(m) => format!("{m}M"),
    }
}

/// Bit rate with EOS scaling: three significant digits and a bps/kbps/
/// Mbps/Gbps unit (`24.7 Mbps`, `3.11 Mbps`, `12.3 kbps`, `0 bps`).
pub fn rate_bps(bps: f64) -> String {
    let (value, unit) = if bps >= 1e9 {
        (bps / 1e9, "Gbps")
    } else if bps >= 1e6 {
        (bps / 1e6, "Mbps")
    } else if bps >= 1e3 {
        (bps / 1e3, "kbps")
    } else {
        (bps, "bps")
    };
    let text = if unit == "bps" || value >= 100.0 {
        format!("{}", value.round() as u64)
    } else if value >= 10.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    };
    format!("{text} {unit}")
}

/// One-decimal percentage: `2.5%`, `0.0%`.
pub fn pct(value: f64) -> String {
    format!("{value:.1}%")
}

/// Verbose duration for the Up/Down line: `12 days, 4 hours, 33 minutes,
/// 12 seconds`, starting at the largest non-zero unit.
pub fn duration_verbose(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let minutes = (secs % 3_600) / 60;
    let seconds = secs % 60;
    let unit = |n: u64, one: &str, many: &str| {
        if n == 1 {
            format!("{n} {one}")
        } else {
            format!("{n} {many}")
        }
    };
    let mut parts = Vec::new();
    if days > 0 {
        parts.push(unit(days, "day", "days"));
    }
    if days > 0 || hours > 0 {
        parts.push(unit(hours, "hour", "hours"));
    }
    if days > 0 || hours > 0 || minutes > 0 {
        parts.push(unit(minutes, "minute", "minutes"));
    }
    parts.push(unit(seconds, "second", "seconds"));
    parts.join(", ")
}

/// Compact duration: `12 days, 4:33:12`, or `0:00:04` under a day.
pub fn duration_compact(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let minutes = (secs % 3_600) / 60;
    let seconds = secs % 60;
    let clock = format!("{hours}:{minutes:02}:{seconds:02}");
    match days {
        0 => clock,
        1 => format!("1 day, {clock}"),
        d => format!("{d} days, {clock}"),
    }
}

/// The rate-window label: `5 minutes` for 300s, `30 seconds` for 30s.
pub fn load_interval_label(secs: u32) -> String {
    if secs >= 60 && secs % 60 == 0 {
        let minutes = secs / 60;
        if minutes == 1 {
            "1 minute".into()
        } else {
            format!("{minutes} minutes")
        }
    } else if secs == 1 {
        "1 second".into()
    } else {
        format!("{secs} seconds")
    }
}

/// The `Intvl` cell of `show interfaces counters rates`. EOS renders the
/// load interval with the *minutes* in the seconds slot (300s prints as
/// `0:05`, meaning 5 minutes) — a quirk preserved deliberately.
pub fn intvl_cell(secs: u32) -> String {
    let minutes = secs / 60;
    format!("{}:{:02}", minutes / 60, minutes % 60)
}

/// Hard truncation at `max` characters — no ellipsis (EOS behavior for
/// the `Name` column of `show interfaces status`).
pub fn truncate_hard(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// Compress a VLAN id list into EOS range notation: `10-12,20`. Input
/// need not be sorted or unique.
pub fn compress_vlans(vlans: &[u32]) -> String {
    let mut ids: Vec<u32> = vlans.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < ids.len() {
        let start = ids[i];
        let mut end = start;
        while i + 1 < ids.len() && ids[i + 1] == end + 1 {
            i += 1;
            end = ids[i];
        }
        parts.push(if start == end {
            format!("{start}")
        } else {
            format!("{start}-{end}")
        });
        i += 1;
    }
    parts.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speeds_format_like_eos() {
        assert_eq!(speed_detail(Some(1000)), "1Gb/s");
        assert_eq!(speed_detail(Some(10_000)), "10Gb/s");
        assert_eq!(speed_detail(Some(100)), "100Mb/s");
        assert_eq!(speed_detail(Some(2500)), "2500Mb/s");
        assert_eq!(speed_detail(None), "Unconfigured");
        assert_eq!(speed_tabular(Some(1000)), "1G");
        assert_eq!(speed_tabular(Some(20_000)), "20G");
        assert_eq!(speed_tabular(Some(100)), "100M");
        assert_eq!(speed_tabular(None), "unconf");
    }

    #[test]
    fn rates_use_three_significant_digits() {
        assert_eq!(rate_bps(24_700_000.0), "24.7 Mbps");
        assert_eq!(rate_bps(3_110_000.0), "3.11 Mbps");
        assert_eq!(rate_bps(96_300_000.0), "96.3 Mbps");
        assert_eq!(rate_bps(1_020_000.0), "1.02 Mbps");
        assert_eq!(rate_bps(12_300.0), "12.3 kbps");
        assert_eq!(rate_bps(8_910.0), "8.91 kbps");
        assert_eq!(rate_bps(210_000_000.0), "210 Mbps");
        assert_eq!(rate_bps(0.0), "0 bps");
        assert_eq!(rate_bps(1_500_000_000.0), "1.50 Gbps");
        assert_eq!(rate_bps(999.0), "999 bps");
    }

    #[test]
    fn verbose_durations_read_naturally() {
        assert_eq!(
            duration_verbose(12 * 86_400 + 4 * 3_600 + 33 * 60 + 12),
            "12 days, 4 hours, 33 minutes, 12 seconds"
        );
        assert_eq!(
            duration_verbose(86_400 + 1),
            "1 day, 0 hours, 0 minutes, 1 second"
        );
        assert_eq!(duration_verbose(3_600), "1 hour, 0 minutes, 0 seconds");
        assert_eq!(duration_verbose(125), "2 minutes, 5 seconds");
        assert_eq!(duration_verbose(9), "9 seconds");
        assert_eq!(duration_verbose(0), "0 seconds");
    }

    #[test]
    fn compact_durations_match_eos() {
        assert_eq!(
            duration_compact(12 * 86_400 + 4 * 3_600 + 33 * 60 + 12),
            "12 days, 4:33:12"
        );
        assert_eq!(duration_compact(4), "0:00:04");
        assert_eq!(duration_compact(86_400 + 3_600), "1 day, 1:00:00");
    }

    #[test]
    fn load_interval_labels() {
        assert_eq!(load_interval_label(300), "5 minutes");
        assert_eq!(load_interval_label(30), "30 seconds");
        assert_eq!(load_interval_label(60), "1 minute");
        assert_eq!(load_interval_label(90), "90 seconds");
    }

    #[test]
    fn intvl_preserves_the_eos_quirk() {
        assert_eq!(intvl_cell(300), "0:05");
        assert_eq!(intvl_cell(1800), "0:30");
        assert_eq!(intvl_cell(3600), "1:00");
    }

    #[test]
    fn truncation_is_a_hard_cut() {
        assert_eq!(
            truncate_hard("uplink LAG to qs-hq-spine", 26),
            "uplink LAG to qs-hq-spine"
        );
        assert_eq!(
            truncate_hard("a very long interface description here", 26),
            "a very long interface desc"
        );
        assert_eq!(truncate_hard("", 26), "");
    }

    #[test]
    fn vlan_lists_compress_to_ranges() {
        assert_eq!(compress_vlans(&[10, 11, 12, 20]), "10-12,20");
        assert_eq!(compress_vlans(&[10, 20, 30, 99]), "10,20,30,99");
        assert_eq!(compress_vlans(&[3, 1, 2]), "1-3");
        assert_eq!(compress_vlans(&[7]), "7");
        assert_eq!(compress_vlans(&[]), "");
        assert_eq!(compress_vlans(&[5, 5, 6]), "5-6");
    }
}
