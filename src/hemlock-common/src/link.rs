//! Link parameters shared by the daemons and the front-ends: the
//! platform's speed/duplex mode tokens and the MTU bounds every layer
//! validates against.
//!
//! A platform manifest declares what a port can do as
//! `supported_modes = ["10M/half", "1G/full", "auto"]`; syncd carries
//! that list into `InterfaceState`, and both hemlockctl and the web
//! console offer exactly those choices. The parsing lives here so the
//! validator (syncd, which owns the port table) and the pickers agree
//! on what a token means.

/// Smallest configurable MTU. Below 68 bytes IPv4 fragment reassembly
/// is not guaranteed (RFC 791), and the kernel refuses it outright.
pub const MIN_MTU: u32 = 68;

/// Largest configurable MTU: the jumbo ceiling the KNET default
/// (`rx_buffer_size=9238`) and the Broadcom XGS families accept.
pub const MAX_MTU: u32 = 9216;

/// The kernel's netdev default, which the management NIC and the SVI
/// bridges boot with. Deleting an `mtu` leaf restores it.
pub const DEFAULT_MTU: u32 = 1500;

/// The MTU front-panel ports boot with: the KNET default the platform
/// loads (`linux-bcm-knet default_mtu=9100`). Deleting a front-panel
/// `mtu` leaf restores it.
pub const DEFAULT_PORT_MTU: u32 = 9100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Duplex {
    Full,
    Half,
}

impl Duplex {
    pub fn as_str(self) -> &'static str {
        match self {
            Duplex::Full => "full",
            Duplex::Half => "half",
        }
    }

    pub fn parse(word: &str) -> Option<Duplex> {
        match word.to_ascii_lowercase().as_str() {
            "full" => Some(Duplex::Full),
            "half" => Some(Duplex::Half),
            _ => None,
        }
    }
}

/// `68..=9216`, the range every MTU setter shares.
pub fn valid_mtu(mtu: u32) -> Result<(), String> {
    if (MIN_MTU..=MAX_MTU).contains(&mtu) {
        Ok(())
    } else {
        Err(format!("bad MTU {mtu} ({MIN_MTU}..{MAX_MTU})"))
    }
}

/// Parse one `supported_modes` token into its forced (Mb/s, duplex)
/// pair. `auto` is not a forced mode and yields `None`, as does any
/// token a manifest spells wrongly.
pub fn parse_mode(token: &str) -> Option<(u32, Duplex)> {
    let (speed, duplex) = token.split_once('/')?;
    Some((parse_speed(speed)?, Duplex::parse(duplex)?))
}

/// Render a forced pair back into manifest spelling (`1G/full`).
pub fn format_mode(speed_mbps: u32, duplex: Duplex) -> String {
    format!("{}/{}", format_speed(speed_mbps), duplex.as_str())
}

/// A rate in manifest spelling: `10M`, `1G`, `10G`. Rates that are not
/// a whole number of gigabits stay in megabits.
pub fn format_speed(speed_mbps: u32) -> String {
    if speed_mbps >= 1000 && speed_mbps % 1000 == 0 {
        format!("{}G", speed_mbps / 1000)
    } else {
        format!("{speed_mbps}M")
    }
}

/// Parse a rate written as bare megabits (`1000`) or with a unit
/// suffix (`1G`, `100m`), case-insensitively.
pub fn parse_speed(word: &str) -> Option<u32> {
    let word = word.trim();
    if let Some(digits) = word.strip_suffix(['G', 'g']) {
        return digits.parse::<u32>().ok()?.checked_mul(1000);
    }
    let digits = word.strip_suffix(['M', 'm']).unwrap_or(word);
    digits.parse::<u32>().ok().filter(|&mbps| mbps > 0)
}

/// The distinct forced rates a port supports, in ascending order —
/// what a `speed` picker offers. `auto` is excluded; ask
/// [`supports_auto`] for that.
pub fn supported_speeds(modes: &[String]) -> Vec<u32> {
    let mut speeds: Vec<u32> = modes
        .iter()
        .filter_map(|m| parse_mode(m).map(|(speed, _)| speed))
        .collect();
    speeds.sort_unstable();
    speeds.dedup();
    speeds
}

/// The distinct duplexes a port supports, full first — what a `duplex`
/// picker offers.
pub fn supported_duplexes(modes: &[String]) -> Vec<Duplex> {
    let mut seen = Vec::new();
    for duplex in [Duplex::Full, Duplex::Half] {
        if modes
            .iter()
            .filter_map(|m| parse_mode(m))
            .any(|(_, d)| d == duplex)
        {
            seen.push(duplex);
        }
    }
    seen
}

/// Does the platform declare auto-negotiation for this port?
pub fn supports_auto(modes: &[String]) -> bool {
    modes.iter().any(|m| m.eq_ignore_ascii_case("auto"))
}

/// Is the forced pair one of the declared modes? A port with no
/// declared modes (a manifest that omits them) accepts anything —
/// refusing every pin would be worse than trusting the operator.
pub fn mode_supported(modes: &[String], speed_mbps: u32, duplex: Duplex) -> bool {
    if modes.iter().all(|m| m.eq_ignore_ascii_case("auto")) {
        return true;
    }
    modes
        .iter()
        .filter_map(|m| parse_mode(m))
        .any(|(s, d)| s == speed_mbps && d == duplex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_manifest_tokens() {
        assert_eq!(parse_mode("1G/full"), Some((1000, Duplex::Full)));
        assert_eq!(parse_mode("10M/half"), Some((10, Duplex::Half)));
        assert_eq!(parse_mode("100M/full"), Some((100, Duplex::Full)));
        assert_eq!(parse_mode("10G/full"), Some((10_000, Duplex::Full)));
        // `auto` is a negotiation marker, not a forced pair.
        assert_eq!(parse_mode("auto"), None);
        assert_eq!(parse_mode("1G"), None);
        assert_eq!(parse_mode("1G/quarter"), None);
    }

    #[test]
    fn round_trips_modes() {
        for token in ["10M/half", "100M/full", "1G/full", "10G/full"] {
            let (speed, duplex) = parse_mode(token).expect("known token");
            assert_eq!(format_mode(speed, duplex), token);
        }
    }

    #[test]
    fn parses_speeds_with_and_without_units() {
        assert_eq!(parse_speed("1000"), Some(1000));
        assert_eq!(parse_speed("1G"), Some(1000));
        assert_eq!(parse_speed("1g"), Some(1000));
        assert_eq!(parse_speed("100M"), Some(100));
        assert_eq!(parse_speed("0"), None);
        assert_eq!(parse_speed("fast"), None);
    }

    #[test]
    fn offers_only_declared_choices() {
        let modes: Vec<String> = ["10M/half", "10M/full", "100M/full", "1G/full", "auto"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(supported_speeds(&modes), vec![10, 100, 1000]);
        assert_eq!(supported_duplexes(&modes), vec![Duplex::Full, Duplex::Half]);
        assert!(supports_auto(&modes));
        assert!(mode_supported(&modes, 1000, Duplex::Full));
        // 1G is full-duplex only on this port, and 10G is not offered.
        assert!(!mode_supported(&modes, 1000, Duplex::Half));
        assert!(!mode_supported(&modes, 10_000, Duplex::Full));
    }

    #[test]
    fn a_manifest_without_modes_accepts_any_pin() {
        assert!(mode_supported(&[], 2500, Duplex::Full));
        assert!(mode_supported(&["auto".to_string()], 2500, Duplex::Full));
    }

    #[test]
    fn mtu_bounds() {
        assert!(valid_mtu(1500).is_ok());
        assert!(valid_mtu(MIN_MTU).is_ok());
        assert!(valid_mtu(MAX_MTU).is_ok());
        assert!(valid_mtu(67).is_err());
        assert!(valid_mtu(9217).is_err());
    }
}
